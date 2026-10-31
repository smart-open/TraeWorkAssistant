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
//!    检测同构）；非流式：QoderTranslate 内联 + `wb_sse::aggregate` 聚合 →
//!    协议投影复用 wb_sse 转换器；
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
use super::wb_upstream::{
    lines_with_first_byte_hedged, lines_with_first_byte_timeout_tracked, new_read_err_slot,
    ReadErrSlot, InterruptibleLines,
};
use super::{ApiSharedState, ErrKind, InflightGuard};
use crate::api_server::routes::{anthropic_error, openai_error, Protocol, stream_msg_or_fallback};
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

/// Qoder 模型级冷却表：model → (冷却截止时刻, 连续失败次数)。排队/超限类错误
/// （ErrMeta 分类 Queued）是模型级现象（换账号同样排队），与账号无关，故进程级
/// 静态表语义等价；挂 ApiSharedState 需改 server.rs（他人负责），静态 OnceLock
/// 最小改动。失败次数仅由 Server 渐进冷却维护（issue #90 修复①），成功即随
/// 条目清除。
/// 备忘（审查 G6）：键为裸 model 名、无区域前缀，v1 恒 CN 区解析无歧义；
/// 接线 Global 区时需加区域前缀（如 "global:{model}"）。表不做主动回收，
/// 残留条目随冷却窗口自然过期，重启即清零。
static QODER_MODEL_COOLDOWNS: OnceLock<Mutex<HashMap<String, (Instant, u32)>>> = OnceLock::new();

fn cooldown_map() -> &'static Mutex<HashMap<String, (Instant, u32)>> {
    QODER_MODEL_COOLDOWNS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Server 类（5xx/业务 504「上游服务异常」）渐进冷却参数（issue #90 修复①）：
/// 30s 起、按失败片段翻倍（30/60/120/240/480/**600**），上限 10 分钟。模型级冷却
/// 窗口内 dispatch 健康预检（dispatch.rs pool_health）快速回退多源模型到 Trae/Buddy
/// 池，不再每请求硬打已知 504 的 qoder 上游；成功即清（clear_model_cooldown）。
const SERVER_COOLDOWN_BASE_SECS: u64 = 30;
const SERVER_COOLDOWN_MAX_SECS: u64 = 600;
/// 翻倍档位上限（移位数）：5 档后 30<<5=960 由 SERVER_COOLDOWN_MAX_SECS 截到 600。
/// 审查修复：此前写死 4（30<<4=480），上限常量 600 永远触不到，注释与实际值自相矛盾
const SERVER_COOLDOWN_MAX_SHIFT: u32 = 5;

/// 记录模型级冷却：按本次排队退避时长（已钳制）设定截止时刻，保留既有连续
/// 失败计数（排队退避与 Server 渐进冷却共用一张表）。冷却窗口内后续请求由
/// dispatch 健康预检快速回退（多源）/显式 429（单源），执行路径跳过同号退避，
/// 不再每请求空转 5s×3~30s×3。
/// 备忘（审查 G6）：键为裸 model 名、无区域前缀（v1 恒 CN 区）；接线 Global
/// 区时需加区域前缀，见 QODER_MODEL_COOLDOWNS 定义处备忘。
fn note_model_cooldown(model: &str, secs: u64) {
    cooldown_map()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(model.to_string())
        // 取较大者：既有 Server 渐进冷却窗口（可能远长于排队建议值）不得被
        // 本次建议值缩短（issue #90 修复①配套语义，queued_cooldown 测试固化）
        .and_modify(|(until, _)| {
            let candidate = Instant::now() + Duration::from_secs(secs);
            if candidate > *until {
                *until = candidate;
            }
        })
        .or_insert_with(|| (Instant::now() + Duration::from_secs(secs), 0));
}

/// Server 类错误（上游 5xx / 业务 504「上游服务异常」）模型级渐进冷却
/// （issue #90 修复①）：冷却窗口 = min(30s × 2^n, 600s)，n = 失败片段数。
/// **递增条件是既有窗口已过期**而非调用次数——流内/HTTP 两路径在 !sent_any
/// 时都会 `break` 逐账号换号，一次请求内连续换号会对同一模型记账多次；若按
/// 调用次数递增，5 账号一次请求即可把窗口推到 480s，把「跨片段渐进」放大成
/// 「单请求轮换放大」，恢复探测被拉长到 8 分钟。按片段计数后：同请求内换号
/// （窗口未过期）不递增，探测节奏 = 冷却窗口（30s→60s→…→10min），成功即清。
fn note_model_server_cooldown(model: &str) {
    cooldown_map()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(model.to_string())
        .and_modify(|(until, fails)| {
            // 既有窗口已过期 = 新一次失败片段 → 递增；未过期 = 同片段内的
            // 换号/并发失败，保持既有截止时刻。
            //
            // 审查修复：此前无论过期与否都把 `until` 推到 now+delay——持续失败流量
            // 会让窗口不断向前滑动，恢复探测被无限推迟（只要请求不停，冷却永不到期），
            // 与「渐进 + 到期探测」的设计相悖。窗口仅在**新片段开始时**重设。
            if Instant::now() < *until {
                if *fails == 0 {
                    // 排队退避落下的短窗（fails=0）：Server 失败应至少抬到基准窗
                    // （5s 排队窗不足以覆盖 5xx），但不递增计数（同片段语义；
                    // fails>=1 的 Server 冷却窗维持「未过期不后延」防滑动）
                    let cand = Instant::now() + Duration::from_secs(SERVER_COOLDOWN_BASE_SECS);
                    if cand > *until {
                        *until = cand;
                    }
                }
                return;
            }
            *fails = fails.saturating_add(1);
            // saturating_sub：fails 可能为 0（note_model_cooldown 排队退避路径
            // or_insert 的 (until, 0) 条目）——裸 `*fails - 1` 会下溢（debug panic /
            // release 回绕致 min 恒取上限、冷却被错误推到最大值）
            let delay = (SERVER_COOLDOWN_BASE_SECS
                << (*fails).saturating_sub(1).min(SERVER_COOLDOWN_MAX_SHIFT))
            .min(SERVER_COOLDOWN_MAX_SECS);
            *until = Instant::now() + Duration::from_secs(delay);
        })
        .or_insert_with(|| {
            (
                Instant::now() + Duration::from_secs(SERVER_COOLDOWN_BASE_SECS),
                1,
            )
        });
}

/// 模型冷却剩余秒数；None = 无冷却（过期残留条目同 wb 版语义按无冷却处理）
pub fn model_cooling_remaining_secs(model: &str) -> Option<u64> {
    let map = cooldown_map().lock().unwrap_or_else(|e| e.into_inner());
    let until = map.get(model)?;
    let left = until.0.saturating_duration_since(Instant::now());
    if left.is_zero() {
        None
    } else {
        Some(left.as_secs())
    }
}

/// 请求成功后清除该模型冷却（同 wb_route::clear_model_failure 语义）；连续
/// 失败计数随条目一并清除（成功 = 该模型恢复服务，渐进冷却重新从基准值起步）
fn clear_model_cooldown(model: &str) {
    cooldown_map()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(model);
}

/// last_error 时间戳（UTC+8，与 app 日志时间线同区，issue #90 修复②）：
/// 504 类上游异常是否持续、何时开始，用户此前只能靠猜（正是「切换模型导致」
/// 误解的成因），带时刻后可与日志逐行对照
fn now_cn_timestamp() -> String {
    use chrono::FixedOffset;
    let cn = FixedOffset::east_opt(8 * 3600).expect("UTC+8 offset is valid");
    chrono::Utc::now()
        .with_timezone(&cn)
        .format("%Y-%m-%d %H:%M:%S")
        .to_string()
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

/// 从上游 usage JSON 提取 Qoder 积分测量（issue #82 T2）：
/// 返回 (credits, original_credits)。credits 取复数字段——单数 `credit` 是旧版
/// 字段，读它恒为 0（qoder2api-hub issue #22 命门），故仅在复数缺失时参与回退。
/// **等价性前提**：与 Python `or` 链「首值非零即用」等价，仅在上游保证单数
/// credit 恒为 0/缺失时成立；两值同现时取 max（按更高积分计）是有意为之的
/// 保守口径，不追求与 or 链逐值等价。usage 缺失或计费字段都取不到 → None
/// （调用方传 None 不入账，与「上游未下发 usage 不得伪造」口径一致）
fn extract_billing(usage: Option<&Value>) -> (Option<f64>, Option<f64>) {
    let Some(u) = usage else {
        return (None, None);
    };
    let num = |key: &str| -> Option<f64> {
        u.get(key)
            .and_then(|v| v.as_f64())
            .filter(|n| n.is_finite() && *n >= 0.0)
    };
    let pick = |a: Option<f64>, b: Option<f64>| match (a, b) {
        (Some(x), Some(y)) => Some(x.max(y)),
        (Some(x), None) => Some(x),
        (None, y) => y,
    };
    let credits = pick(num("credits"), num("credit"));
    let original = pick(num("original_credits"), num("original_credit"));
    (credits, original)
}

/// system-reminder 剥离正则（惰性编译一次）：非贪婪跨行匹配整块注入。
fn reminder_re() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(r"(?s)\s*<system-reminder>.*?</system-reminder>\s*")
            .expect("system-reminder regex")
    })
}

/// user 输入包裹标签正则（惰性编译一次）：<user_input> / </user_input>（容忍属性）。
fn user_input_tag_re() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(r"</?user_input[^>]*>").expect("user_input tag regex")
    })
}

/// 剥掉客户端注入的 `<system-reminder>` 块与 `<user_input>` 包裹标签（agent
/// 客户端把它们拼进 user 消息——reminder 前置劫持正文、输入被标签包裹），
/// 剩余即用户原话。剥离后仅剩空白时返回空串（调用方按无 preview 处理）。
fn strip_reminders(text: &str) -> String {
    let no_reminders = reminder_re().replace_all(text, " ");
    let stripped = user_input_tag_re().replace_all(&no_reminders, " ");
    // 连续空白压成单个空格后 trim（多块剥离后常残留空行/空格）
    let mut out = String::with_capacity(stripped.len());
    let mut in_ws = false;
    for ch in stripped.chars() {
        if ch.is_whitespace() {
            in_ws = true;
        } else {
            if in_ws && !out.is_empty() {
                out.push(' ');
            }
            in_ws = false;
            out.push(ch);
        }
    }
    out
}

/// issue #82 问题5：会话 preview——末条 user 消息文本（与 build_upstream_body
/// last_user_text 同款逆序口径），剥离注入块/包裹标签后截 200 字符。
fn session_preview(peek: &Value) -> String {
    let mut text = String::new();
    if let Some(messages) = peek.get("messages").and_then(Value::as_array) {
        for message in messages.iter().rev() {
            if message.get("role").and_then(Value::as_str) == Some("user") {
                text = qoder_upstream::content_to_text(
                    message.get("content").unwrap_or(&Value::Null),
                );
                break;
            }
        }
    }
    strip_reminders(&text).chars().take(200).collect()
}

/// issue #82 问题5：失败路径记账会话上下文。失败时本无上游信封可解析
/// （两处调用恒为占位），恒退化为 uuid 占位（request_id 仅幂等用途，占位不
/// 影响正确性）。成功路径由预解析 id 走 session_ctx_from_ids，不经本函数。
fn session_ctx(peek: &Value) -> super::usage::QoderSessionCtx {
    super::usage::QoderSessionCtx::placeholder(session_preview(peek))
}

/// issue #82 审查修复 K：信封顶层 request_id/session_id 提取。信封含完整对话历史
/// （大 body），内层重试循环若每次记账都全量反序列化属热路径冗余；
/// id 随信封恒定，应在信封生成后解析一次、循环内复用（本函数即一次解析的产物源）。
/// 会话聚合扩展：三元组带 chat_record_id（轮键，「展开会话看各轮明细」分组用）。
fn extract_envelope_ids(body: &[u8]) -> Option<(String, String, String)> {
    let v = serde_json::from_slice::<Value>(body).ok()?;
    let get = |k: &str| {
        v.get(k)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    let (rid, sid, rec) = (get("request_id"), get("session_id"), get("chat_record_id"));
    (!rid.is_empty()).then_some((rid, sid, rec))
}

/// 由预解析的信封 id 构造记账上下文（id 为 None = 无信封/解析失败 → uuid 占位）
fn session_ctx_from_ids(
    peek: &Value,
    ids: Option<&(String, String, String)>,
) -> super::usage::QoderSessionCtx {
    match ids {
        Some((rid, sid, rec)) => super::usage::QoderSessionCtx {
            request_id: rid.clone(),
            session_id: sid.clone(),
            // 轮键（信封 chat_record_id，移植自 main 11d23c1 会话聚合扩展）：
            // 同一段消息+模型+输出上限派生同值，「展开会话看各轮明细」分组用
            record_id: rec.clone(),
            ts: chrono::Local::now().timestamp(),
            preview: session_preview(peek),
        },
        None => super::usage::QoderSessionCtx::placeholder(session_preview(peek)),
    }
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

/// 请求体是否携带图片输入（issue #154 门控）：三协议形态统一识别——
/// OpenAI `image_url` / Anthropic `image` / Responses `input_image`。
/// 数组 content 递归一层（覆盖 tool_result.content 内嵌图片），字符串恒无图。
fn body_has_image(body: &Value) -> bool {
    body.get("messages")
        .and_then(Value::as_array)
        .is_some_and(|messages| {
            messages
                .iter()
                .any(|m| m.get("content").is_some_and(content_has_image))
        })
}

fn content_has_image(content: &Value) -> bool {
    match content {
        Value::Array(items) => items.iter().any(|item| {
            matches!(
                item.get("type").and_then(Value::as_str),
                Some("image_url") | Some("image") | Some("input_image")
            ) || item.get("content").is_some_and(content_has_image)
        }),
        _ => false,
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

/// 信封业务 400（HTTP 200 + statusCodeValue=400）且本请求下发了 context_length
/// 覆盖 → 终局错误文案（原错误 + 清空设置指引），与 HTTP 层 Fatal 同前缀格式；
/// 否则 None 走原有轮换。参数拒绝与账号无关：换号只会逐号复现并误冷却账号，
/// 最终以「无健康账号」收尾吞掉可行动的指引。仅接 Unknown——排队/鉴权/额度
/// 业务码各有既定语义，不在此劫持
fn envelope_context_reject(entry: &Value, meta: Option<&ErrMeta>) -> Option<String> {
    let meta = meta.filter(|m| m.kind == UpstreamKind::Unknown)?;
    let hint = qoder_upstream::context_override_hint(entry, meta.status);
    if hint.is_empty() {
        return None;
    }
    let detail = if meta.raw.is_empty() {
        meta.message.clone()
    } else {
        format!("{}: {}", meta.message, safe_slice(&meta.raw, 300))
    };
    Some(format!("upstream {} error: {detail}{hint}", meta.status))
}

// ==================== F-80-余 v2 慢请求竞速对冲（取号侧编排，wb_route 同构） ====================

/// 对冲账号在途计数租约（镜像 wb_route::HedgeLease）：构造即 +1（竞速窗口
/// 占用），Drop 即 -1——建连/凭证解析失败（闭包内提前返回）、双败、竞速胜出
/// 三类退出路径均恰好释放一次，杜绝计数泄漏导致的账号永久 busy
struct QoderHedgeLease {
    uid: String,
    counter: std::sync::Arc<std::sync::atomic::AtomicU32>,
    /// 对冲账号信封的 (request_id, session_id, record_id)（G4）：对冲请求按对冲账号 uid
    /// 重建信封，两账号信封 id 不同——接管记账必须落对冲账号信封的 id，
    /// 否则行 uid=对冲账号而会话键属主账号信封，归属错位
    session_ids: Option<(String, String, String)>,
}

impl QoderHedgeLease {
    fn acquire(
        state: &ApiSharedState,
        uid: &str,
        session_ids: Option<(String, String, String)>,
    ) -> Self {
        let counter = state.qoder_pool.inflight_handle(uid);
        counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Self { uid: uid.to_string(), counter, session_ids }
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
    /// 胜者信封的 (request_id, session_id, record_id)（G4）：主账号胜 = 主信封 id，
    /// 对冲接管 = 对冲信封 id；记账按胜者落定，与会话行 uid 同源
    session_ids: Option<(String, String, String)>,
    /// 审查修复（L2）：竞速败者已死标记（EOF/IO/超窗，随 RaceOutcome 透传），
    /// settle_qoder_hedge 据此对败者补记错误/冷却（镜像 wb_route::settle_hedge）
    primary_failed: bool,
    hedge_failed: bool,
    /// 胜者行源读错误共享槽（issue #158 P2-2）：QoderTranslate 据此在 EOF 时
    /// 识别传输层掐断（不补 [DONE] + 写 TransportCut ErrMeta）
    read_slot: ReadErrSlot,
}

/// 首字竞速（F-80-余 v2，镜像 wb_route::race_first_byte）：对冲关闭（阈值 0）
/// 时与纯首字超时语义完全一致；开启时主请求首字节超阈值 → 从池内取第二账号
/// （走同一 busy 过滤——主账号已 inflight 天然让位，受 F-77 并发上限约束）
/// 发对冲请求，先出首字者胜。对冲请求独立解析凭证（identity 回调）并按对冲
/// 账号 uid 重建 agent 信封（session 派生依赖 uid）。
/// `primary_session_ids`：主账号信封的 (request_id, session_id, record_id)
/// （外层已解析一次）；审查修复 G4：胜出判定后按胜者连接落定
/// QoderRaceWin.session_ids——接管时取对冲信封 ids（随租约带回），否则取主
/// 信封 ids，保证记账归属一致。
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
    primary_session_ids: &Option<(String, String, String)>,
    converted_len: usize,
) -> Result<QoderRaceWin, ()> {
    let hedge_ms = state
        .qoder_hedge_threshold_ms
        .load(std::sync::atomic::Ordering::Relaxed);
    // issue #138：对冲关闭（hedge_ms==0）或大 body（> HEDGE_SKIP_BODY_BYTES，
    // 对冲需整包克隆 peek + 按 hedge 账号 prepare_qoder_body 重建，瞬时内存
    // 放大得不偿失）时均退化为纯首字超时（语义与关闭对冲一致）
    if hedge_ms == 0 || converted_len > super::routes::HEDGE_SKIP_BODY_BYTES {
        // P2-2：tracked 变体——行源被 Box 擦拭类型后读错误须经共享槽跨层读取
        let read_slot = new_read_err_slot();
        let lines = lines_with_first_byte_timeout_tracked(primary_reader, read_slot.clone())?;
        return Ok(QoderRaceWin {
            lines,
            uid: primary_uid.to_string(),
            hedge: None,
            takeover: false,
            session_ids: primary_session_ids.clone(),
            primary_failed: false,
            hedge_failed: false,
            read_slot,
        });
    }
    let state2 = state.clone();
    let tried2 = tried.clone();
    // issue #111：对冲取号与主路径同口径——免费模型豁免积分维度过滤
    let free2 = qoder_upstream::entry_is_free(entry);
    let allowed2 = allowed.cloned();
    let dedicated2 = dedicated.map(str::to_string);
    let peek2 = peek.clone();
    let entry2 = entry.clone();
    let mk2 = model_key.to_string();
    let ms2 = model_source.to_string();
    // 审查修复（L2）：捕获对冲账号 uid（仅建连成功 = 对冲实际发起时记录）——
    // 双败 Err 路径租约随竞速 Drop、无从取 uid，Err 时据此对对冲侧补记错误
    let hedge_uid_cell = std::rc::Rc::new(std::cell::RefCell::new(None::<String>));
    let hedge_uid_slot = hedge_uid_cell.clone();
    let spawn_backup = move || -> Option<(Box<dyn std::io::Read + Send>, QoderHedgeLease)> {
        let (picked2, ev) = state2
            .qoder_pool
            .pick_excluding_constrained_ev(&tried2, allowed2.as_ref(), dedicated2.as_deref(), free2)?;
        // F-77⑤ 可观测：对冲取号同样记录 busy 让位/降级事件
        if let Some(ev) = ev {
            state2.logger.log_sched_event(&ev);
        }
        // 租约先于凭证解析获取：失败路径随闭包局部变量 Drop 自动 -1
        let creds2 = (state2.qoder_identity.as_ref()? )(&picked2.uid).ok()?;
        let converted2 = qoder_upstream::prepare_qoder_body(&peek2, &entry2, &creds2.uid).ok()?;
        // G4：对冲信封 id 随租约带出，接管记账按对冲账号信封落定
        let session_ids = extract_envelope_ids(&converted2);
        let lease = QoderHedgeLease::acquire(&state2, &picked2.uid, session_ids);
        let reader2 = qoder_upstream::make_qoder_request(&creds2, &converted2, &mk2, &ms2).ok()?;
        *hedge_uid_slot.borrow_mut() = Some(picked2.uid.clone());
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
            // G4：会话 id 按胜者信封落定——接管取对冲信封 id，主胜取主信封 id
            let session_ids = if out.takeover {
                out.hedge.as_ref().and_then(|l| l.session_ids.clone())
            } else {
                primary_session_ids.clone()
            };
            Ok(QoderRaceWin {
                lines: out.lines,
                uid,
                hedge: out.hedge,
                takeover: out.takeover,
                session_ids,
                primary_failed: out.primary_failed,
                hedge_failed: out.hedge_failed,
                read_slot: out.read_slot,
            })
        }
        Err(()) => {
            // 审查修复（L2）：双败时对冲侧同样记 Server 错误（主侧由调用方按
            // picked.uid 记，与本处对齐）——镜像 wb_route::race_first_byte
            if let Some(huid) = hedge_uid_cell.borrow().as_deref() {
                state.qoder_pool.note_error(huid, ErrKind::Server);
            }
            Err(())
        }
    }
}

/// 竞速结束后处理对冲计数与日志（镜像 wb_route::settle_hedge）：
/// 释放对冲账号竞速窗口占用；接管时 guard 重绑到对冲账号；[SCHED] 日志记录
/// hedge_takeover / hedge_lost；审查修复（L2）：竞速败者记账——已死一侧
/// （EOF/IO/超窗）补记 Server 错误触发冷却，仅「输竞速但未死」一侧不记
fn settle_qoder_hedge(
    state: &ApiSharedState,
    win: &mut QoderRaceWin,
    mut guard: InflightGuard,
    primary_uid: &str,
) -> InflightGuard {
    if win.primary_failed {
        state.qoder_pool.note_error(primary_uid, ErrKind::Server);
    }
    if win.hedge_failed {
        if let Some(lease) = win.hedge.as_ref() {
            state.qoder_pool.note_error(&lease.uid, ErrKind::Server);
        }
    }
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
    // P2-2：胜者读错误槽随行源移交翻译器——EOF 伴随读错误时翻译层不补
    // [DONE] 并写入 TransportCut ErrMeta（见 QoderTranslate::flush_finish）
    let translated = qoder_upstream::QoderTranslate::with_read_slot(
        win.lines,
        err_slot,
        Some(win.read_slot),
        chat_id,
        model,
    );
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
    // issue #154 图片门控：Qoder 上游是 agent 信封，图片走独立 image_urls 上传
    // 通道（本项目未接线），messages 里的图片块上游读不到——与其静默丢图
    //（F-63 红线：能力缺失必须明示），不如执行入口直接 400 拒绝
    if body_has_image(&peek) {
        let msg = "upstream 400 error: Qoder 池暂不支持图片输入（图片上传通道未接线，issue #154），请移除图片或改用支持视觉的非 Qoder 模型";
        state.logger.log_request(
            "qoder", "POST", "/v1/chat/completions", model, true, 400, "-",
            start_ts.elapsed().as_millis() as u64, &key_name, "", Some(&msg),
        );
        send_stream_error(tx, proto, 400, &msg);
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
    // issue #111：免费档模型（倍率 0）零消耗，取号豁免积分维度过滤
    //（零积分/保留阈值/积分过期），账号级状态过滤保持不变。
    // 免费活动覆盖（资源调度页开关）：在地区专属判定之后应用——
    // entry_is_global_only 比较原始条目相等性，提前改写会破坏判定
    let entry = {
        let mut e = entry;
        qoder_upstream::apply_flash_free_override(
            &mut e,
            state
                .qoder_flash_free_rate
                .load(std::sync::atomic::Ordering::Relaxed),
        );
        // Qoder 页单模型上下文长度 → parameters.context_length（未设置不下发）
        qoder_upstream::apply_context_override(
            &mut e,
            super::context_window::qoder_target(&state.data_dir, model),
        );
        e
    };
    let free_model = qoder_upstream::entry_is_free(&entry);

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
                state.qoder_pool.pick_by_uid(&b.uid, free_model).map(|_| b.uid)
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
            .pick_sticky_yield(u, allowed_set.as_ref(), free_model)
            .map(|(p, ev)| {
                if let Some(ev) = ev {
                    state.logger.log_sched_event(&ev);
                }
                p
            })
    });

    // ── issue #158 P1-1：Qoder 池请求侧正向改写（sanitize+模板+工具别名）──
    // 池级开关默认开，UI「指纹清洗」tab 热切换；模板/别名表与 Trae 池共用
    // 同一份配置（wb_template_map.json / 工具别名规则）
    let sanitize = state.qoder_sanitize.load(std::sync::atomic::Ordering::Relaxed);
    let templates = super::wb_route::load_templates(&super::wb_route::WbView::wb(state.clone()));
    let tool_alias_rules =
        super::wb_route::load_tool_alias_rules(&super::wb_route::WbView::wb(state.clone()));
    let mut last_residual_note: Option<String> = None;
    // 会话稳定种子（与 Trae 的 now_ts 不同）：Qoder 的 session_seed 兜底走
    // first_user_fingerprint(messages)——清洗文本必须跨请求一致，否则同会话
    // 每请求派生不同上游 session，破坏会话延续与缓存复用。种子从粘性键派生
    // （SessionKey 派生 Debug），同会话恒定
    let seed_base = format!("q#{:?}", sticky_key);

    let mut tried: HashSet<String> = HashSet::new();
    let mut empty_hits: usize = 0; // 空完成计数：轮换耗尽收尾区分指纹拦截与账号耗尽
    // PR #162：传输掐断计数——全池被掐时耗尽收尾改报传输掐断而非误报无健康账号
    let mut cut_hits: usize = 0;
    // issue #156：轮换总预算 deadline（45s）。首字超时 10s×N 账号的逐号轮换
    // 会拖过客户端 60s 读超时窗口——TRAE 不等终局错误直接报
    // "Timeout after 60000ms (4028)"（issue 贴图），用户只见超时不见原因。
    // 预算内正常轮换不受影响；超预算立即按 exhaust 语义收尾，把「换号重演」
    // 变成「可读错误必达」（配合 hard_credit_stats 文案携带积分耗尽提示）
    let rotate_deadline = Instant::now() + Duration::from_secs(45);
    // M2 审查修复：本请求内已记入模型级 Server 冷却的标记。冷却早退检查针对
    // 的是「并发请求刚记入的窗口」（避免 N 账号逐个硬打已知故障上游）；若不
    // 区分，本请求给首账号记的冷却会在下一轮循环顶立即命中——单次瞬时 5xx/
    // 首字超时即终结整轮轮换，其余账号一次不试。置位后本请求内放行（仍受
    // 45s 预算约束），后续请求照常快速失败
    let mut self_noted_server_cooldown = false;

    loop {
        // 客户端断连检测：通道关闭即终止轮换/重试（对齐 run_wb_stream）。
        // 审查 M5：断连也是请求终局，补记一行失败用量（未取号 → "none"，
        // 与下方轮换耗尽收尾同口径），防用量页 requests 缺口
        if tx.is_closed() {
            state.record_usage_qoder(
                model, "none", key_id, false, true,
                start_ts.elapsed().as_millis() as u64, 0, 0, None, None, None,
                &session_ctx(&peek),
            );
            return;
        }
        // 轮换预算检查（issue #156）：仅约束「换号再试」——首个账号（tried 空）
        // 不受影响；超预算后不再取新号，按 exhaust 语义收尾（错误消息在下一
        // 分支取号失败路径统一拼装，tried 非空保证healthy 统计口径正确）
        if !tried.is_empty() && Instant::now() > rotate_deadline {
            let duration_ms = start_ts.elapsed().as_millis() as u64;
            state.record_usage_qoder(model, "none", key_id, false, true, duration_ms, 0, 0, None, None, None, &session_ctx(&peek));
            let (healthy_total, _) = state.qoder_pool.selectable_stats_in(None, free_model);
            let (hard_credit_cnt, _) = state.qoder_pool.hard_credit_stats();
            let (exhaust_msg, exhaust_tag) = super::routes::exhaust_message_with_transport_cut(
                "qoder",
                empty_hits,
                tried.len(),
                healthy_total,
                hard_credit_cnt,
                cut_hits,
            );
            // 模型级 Server 冷却与指纹判定同口径：空完成主导（指纹拦截）时模型
            // 本身可用（拦的是请求指纹，换号无效），不记 Server 冷却——否则指纹
            // 拦截被误判为模型故障，后续请求被冷却误伤且跨池回退也拦不住同指纹。
            // 仅非指纹主导（真 5xx/超时占多数）才补记冷却，让后续请求走预检
            // 快速失败/跨池回退，不再重演整轮轮换（issue #156）
            if exhaust_tag != "CHANNEL_FINGERPRINT_BLOCK" {
                note_model_server_cooldown(model);
            }
            state.logger.log_request(
                "qoder", "POST", "/v1/chat/completions", model, true, 503, "none",
                duration_ms, &key_name, "", Some(&exhaust_msg),
            );
            // issue #158 P1：池级指纹拦截短记忆（对齐 trae/buddy 池）——空完成主导
            // （指纹拦截）才标记，TTL 600s 内同池新请求激进清洗起步，不再从零试探
            super::wb_payload::mark_channel_fp_block("qoder", exhaust_tag == "CHANNEL_FINGERPRINT_BLOCK");
            let sample_note = super::wb_payload::empty_sample_note(&exhaust_tag, "qoder", body_vec);
            state.logger.log_debug_line(format!(
                "[DEBUG] {exhaust_tag} tried={} empty_hits={} cut_hits={} pool=qoder rotate_budget_exhausted{sample_note}",
                tried.len(),
                empty_hits,
                cut_hits,
            ));
            // 按客户端协议分流错误帧（Anthropic 收 event: error、Responses 收
            // response.failed，不再裸 data: 帧不可读）；code 保持字符串
            // "no_healthy_account"（客户端兼容）
            send_stream_error_named(tx, proto, "no_healthy_account", &exhaust_msg);
            return;
        }
        // 模型级冷却中（并发请求刚记入窗口）：不再逐账号硬打已知故障的上游
        // （issue #90 审查修复）。此前仅跳过同号退避、仍会 break 到下一个账号发
        // 真实请求——N 账号 = N 次无效上游调用，与本条 Feature「不再每请求硬打」
        // 直接相悖。首个账号尚未尝试时不拦截（保留一次探测）。
        // M2：本请求自记的冷却（self_noted_server_cooldown）不触发本早退——
        // 放行继续轮换（受 45s 预算约束），否则首个账号瞬时故障即终结整轮。
        if !tried.is_empty() && !self_noted_server_cooldown {
            if let Some(rem) = model_cooling_remaining_secs(model) {
                let msg = format!("model {model} cooling down, retry after {rem}s");
                // M6 审查收口：错误帧按客户端协议分流（Anthropic event:error、
                // Responses response.failed）——此前裸 data: 帧对这两类客户端
                // 不可读（流静默中断），是本批「错误帧按协议分流」声明的遗漏
                // 分支；code 保持字符串客户端兼容
                send_stream_error_named(tx, proto, "model_cooldown", &msg);
                let duration_ms = start_ts.elapsed().as_millis() as u64;
                state.logger.log_request(
                    "qoder", "POST", "/v1/chat/completions", model, true, 429, "none",
                    duration_ms, &key_name, "", Some(&msg),
                );
                return;
            }
        }
        // ── 取号：粘性命中优先，否则按 Key 约束 + 调度策略；换号后仅走策略 ──
        let picked = match first_pick.take() {
            Some(p) => p,
            None => match state
                .qoder_pool
                .pick_excluding_constrained_ev(&tried, allowed_set.as_ref(), dedicated.as_deref(), free_model)
            {
                Some((p, ev)) => {
                    if let Some(ev) = ev {
                        state.logger.log_sched_event(&ev);
                    }
                    p
                }
                None => {
                    let duration_ms = start_ts.elapsed().as_millis() as u64;
                    state.record_usage_qoder(model, "none", key_id, false, true, duration_ms, 0, 0, None, None, None, &session_ctx(&peek));
                    // 审查 P2：轮换耗尽收尾对齐 routes.rs（issue #57 可观测）——
                    // 空完成主导 → 明示渠道指纹拦截语义（账号/模型无关，提示更新
                    // wb_template_map.json），否则维持无健康账号文案；
                    // code 保持 no_healthy_account（客户端兼容）
                    let (healthy_total, _) = state.qoder_pool.selectable_stats_in(None, free_model);
                    let (hard_credit_cnt, _) = state.qoder_pool.hard_credit_stats();
                    let (exhaust_msg, exhaust_tag) = super::routes::exhaust_message_with_transport_cut(
                        "qoder",
                        empty_hits,
                        tried.len(),
                        healthy_total,
                        hard_credit_cnt,
                        cut_hits,
                    );
                    state.logger.log_request(
                        "qoder", "POST", "/v1/chat/completions", model, true, 503, "none",
                        duration_ms, &key_name, "", Some(&exhaust_msg),
                    );
                    // issue #158 P1：池级指纹拦截短记忆（对齐 trae/buddy 池）——空完成
                    // 主导（指纹拦截）才标记，TTL 600s 内同池新请求激进清洗起步
                    super::wb_payload::mark_channel_fp_block("qoder", exhaust_tag == "CHANNEL_FINGERPRINT_BLOCK");
                    // issue #99 ① + #125 增强：影子风控收尾采样入内存环形缓冲
                    // （kind=empty），返回日志注记供反查漏网触发句
                    let sample_note =
                        super::wb_payload::empty_sample_note(&exhaust_tag, "qoder", body_vec);
                    state.logger.log_debug_line(format!(
                        "[DEBUG] {exhaust_tag} tried={} empty_hits={} cut_hits={} pool=qoder{sample_note}",
                        tried.len(),
                        empty_hits,
                        cut_hits,
                    ));
                    // 按客户端协议分流错误帧（同预算耗尽收尾口径）
                    send_stream_error_named(tx, proto, "no_healthy_account", &exhaust_msg);
                    return;
                }
            },
        };
        tried.insert(picked.uid.clone());
        // 当前账号排队重试计数（账号局部：换号自然重置）
        let mut queue_same: u32 = 0;
        // F-77 账号级在途计数：取号即绑定（换号时 bind_account 自动解绑旧账号）。
        // 审查 P1-4 TOCTOU 收紧：普通候选 CAS 复核「在途 < 并发上限」，已满则
        // 不发起请求换下一候选（不计错误不冷却）；over_limit 候选（粘性锁定、
        // busy_fallback 降级）保留原无条件绑定语义。
        // active_uid 仅在绑定成功后更新（CAS 拒绝的候选不得污染「当前账号」展示）
        let bound = if picked.over_limit {
            guard = guard.bind_account(state.qoder_pool.inflight_handle(&picked.uid));
            true
        } else {
            guard.bind_account_cas(
                state.qoder_pool.inflight_handle(&picked.uid),
                state.qoder_pool.concurrency_limit(),
            )
        };
        if !bound {
            continue;
        }
        *safe_lock(&state.active_uid) = Some(picked.uid.clone());

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
                    // issue #67（移植 main@36d628f）：硬冷却/禁用类错误解绑该账号粘性
                    if kind.unbinds_sticky() {
                        state.qoder_sticky.unbind_uid(&picked.uid);
                    }
                    // 审查修复：凭证解析错误的原文可能携带 PAT / 上游响应片段 /
                    // 临时令牌，与其它分支统一按 safe_slice 截断后再进 last_error
                    //（last_error 会对外展示）——此前此处是唯一不打码的错误出口
                    *safe_lock(&state.last_error) = Some(format!(
                        "qoder identity uid={} err={}",
                        picked.uid,
                        safe_slice(&e, 200)
                    ));
                    state.logger.log_request(
                        "qoder", "POST", "/v1/chat/completions", model, true, 503, &picked.uid,
                        start_ts.elapsed().as_millis() as u64, &key_name,
                        &state.qoder_pool.name_of(&picked.uid), Some("凭证解析失败"),
                    );
                    continue;
                }
            },
        };

        // ── issue #158 P1-1：active_peek（清洗副本）——原始 peek 保留给
        // session_ctx 记账 preview 与 body_has_image；清洗+别名改写打在副本上，
        // 供信封构造与对冲竞速共用（主/对冲指纹一致，竞速不浪费）。
        // 激进门镜像 trae 池 routes.rs：空完成后 / 池级指纹拦截 TTL 内时全力
        // 清洗；种子 tried.len()-1 → 首轮恒 0（会话稳定种子跨请求一致，保护
        // session_seed 的 first_user_fingerprint 兜底），换号递增逃逸方差
        let aggressive =
            sanitize && (empty_hits > 0 || super::wb_payload::fp_block_recent("qoder"));
        let mut active_peek: Value = peek.clone();
        // 逆映射供响应侧回改（P1-2 stream_forward_ex 接线后消费）
        let tool_alias: Option<std::collections::HashMap<String, String>>;
        if sanitize {
            let seed =
                super::wb_payload::variant_seed(&seed_base, (tried.len() - 1) as u64);
            active_peek =
                super::payload::sanitize_openai_body(&peek, &templates, seed, aggressive);
            tool_alias = super::wb_payload::apply_tool_alias_forward_value(
                &mut active_peek,
                &tool_alias_rules,
            );
            // 残指纹自检（issue #122 C3）：改写后首条 system 仍命中已知特征词 →
            // debug 日志（按请求去重），提示模板表需经 UI「指纹清洗」tab 热更新
            if let Some(note) = super::wb_payload::residual_fingerprint_note_value(&active_peek) {
                if last_residual_note.as_deref() != Some(note.as_str()) {
                    last_residual_note = Some(note.clone());
                    state.logger.log_debug_line(format!(
                        "[DEBUG] 残指纹自检命中: {note} pool=qoder uid={}",
                        picked.uid
                    ));
                }
            }
        } else {
            tool_alias = None;
        }

        // 请求体（agent 信封；依赖账号 uid 派生 session）
        let converted = match qoder_upstream::prepare_qoder_body(&active_peek, &entry, &creds.uid) {
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

        // 审查修复 K：信封含完整对话历史（大 body），request_id/session_id 随信封恒定，
        // 在外层换号循环内解析一次供内层各次尝试记账复用，避免热路径重复全量反序列化
        let envelope_ids = extract_envelope_ids(&converted);

        let mut same_attempt: u32 = 0;
        // 对冲接管标记（本轮尝试内有效）：接管后排队同号退避不再适用——
        // 退避重试走主账号 creds/converted，与生效（对冲）账号不一致。
        // 初值在 Ok 分支必然先赋值后读取，lint 对初值误报，显式允许
        #[allow(unused_assignments)]
        let mut hedge_taken = false;
        loop {
            // 断连检测：重试等待/长路径期间离开则终止（guard 随 Drop 释放）。
            // 审查 M5：断连为请求终局，补记一行失败用量（已绑定 picked 账号）
            if tx.is_closed() {
                state.record_usage_qoder(
                    model, &picked.uid, key_id, false, true,
                    start_ts.elapsed().as_millis() as u64, 0, 0, None, None, None,
                    &session_ctx(&peek),
                );
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
                        &active_peek,
                        &entry,
                        &model_key,
                        &model_source,
                        &tried,
                        allowed_set.as_ref(),
                        dedicated.as_deref(),
                        &envelope_ids,
                        converted.len(),
                    ) {
                        Ok(w) => w,
                        Err(()) => {
                            state.qoder_pool.note_error(&picked.uid, ErrKind::Server);
                            // issue #156：首字超时补记模型级 Server 冷却。原实现只
                            // note 账号（30min 软熔断）——后续请求仍会逐账号重演
                            // 10s 首字等待，N 账号 = 10s×N；轮换超 60s 时 TRAE 客户
                            // 端先报 Timeout(4028)（issue 贴图），且一次慢响应毒化
                            // 整池。补模型级冷却后，后续请求走 dispatch 预检快速
                            // 失败/跨池回退，不再重演整轮轮换
                            note_model_server_cooldown(model);
                            self_noted_server_cooldown = true; // M2：本请求自记，放行继续轮换
                            break;
                        }
                    };
                    let ttfb_ms = ttfb_start.elapsed().as_millis() as u64;
                    // 对冲计数落定 + guard 重绑（接管时生效账号 = 对冲账号）
                    guard = settle_qoder_hedge(state, &mut race, guard, &picked.uid);
                    hedge_taken = race.takeover;
                    let win_uid = race.uid.clone();
                    // G4：记账会话 id 按胜者信封落定（race 在 translate 处被消费，先取快照）
                    let win_session_ids = race.session_ids.clone().or_else(|| envelope_ids.clone());
                    // 竞速胜者行源（原始上游行）统一翻译后喂 stream_forward_ex
                    let ilines = translate_race_lines(race, err_slot.clone(), chat_id, model);
                    // Qoder 无 namespace 展平编码（F4 移植）→ 空集合禁用编码名拆分
                    let (error_info, sent_any, failed_inline, usage) =
                        // Qoder 无 namespace 展平编码（Responses 分支为防御性兜底，
                        // 实际仅 OpenAi/Anthropic 可达）→ 空集合禁用编码名拆分（审查 F4）
                        wb_sse::stream_forward_ex(
                            ilines, tx, proto, chat_id, model, &std::collections::HashSet::new(),
                            tool_alias.as_ref(),
                        );
                    let duration_ms = start_ts.elapsed().as_millis() as u64;
                    let meta = err_slot
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .clone();
                    // M1 修复（审查）：as_u64 窄口径对字符串/浮点数字段形态静默
                    // 得 0——改用 wb_sse::u64_pair 宽口径（与 wb/Trae 记账同源）
                    let (pt, ct) = usage.as_ref().map(wb_sse::u64_pair).unwrap_or((0, 0));
                    // M4：缓存读 tokens 同源宽口径提取，终态旁路记账（0 值不入账）
                    let cached = usage.as_ref().map(wb_sse::cached_tokens_of).unwrap_or(0);
                    let (credits, original_credits) = extract_billing(usage.as_ref());
                    // issue #111：免费档调度（免费活动覆盖开启或目录价 0）消耗按 0
                    // 落账——上游 billable=false 实际不扣分，名义值照记会虚增用量
                    let (credits, original_credits) = if free_model {
                        (Some(0.0), Some(0.0))
                    } else {
                        (credits, original_credits)
                    };
                    // 记账改为「终态落一行」（审查修复）：此前每次尝试都无条件调用——
                    // 失败后 `break` 换号的中间态也记一次，同一逻辑请求被记成
                    // N 次（requests/errors 虚高），与非流式「每请求一行、中间态不记」
                    // 口径不一致。改为在下方三个终态点各记一次。
                    let usage_ctx =
                        session_ctx_from_ids(&peek, win_session_ids.as_ref());
                    let record_usage = |ok: bool| {
                        state.record_usage_qoder(
                            model,
                            &win_uid,
                            key_id,
                            ok,
                            true,
                            duration_ms,
                            pt,
                            ct,
                            Some(ttfb_ms),
                            credits,
                            original_credits,
                            // 审查修复 G4：session_ctx 用胜者信封 ids（接管时=对冲侧）
                            &usage_ctx,
                        );
                    };
                    match error_info {
                        Some((code, msg)) => {
                            // 信封 400 拒绝 context_length 覆盖：终局下发原错误 + 清空
                            // 指引（不换号不冷却，同 HTTP 层 Fatal 口径）
                            if !sent_any {
                                if let Some(full) = envelope_context_reject(&entry, meta.as_ref()) {
                                    record_usage(false);
                                    state.logger.log_request_ttfb(
                                        "qoder", "POST", "/v1/chat/completions", model, true,
                                        400, &win_uid, duration_ms, Some(ttfb_ms), &key_name,
                                        &state.qoder_pool.name_of(&win_uid), Some(&full),
                                    );
                                    send_stream_error(tx, proto, 400, &full);
                                    return;
                                }
                            }
                            // 流内错误分类以 ErrMeta 为准（translate 写入，含排队/额度信号）
                            match meta.as_ref().map(|m| m.kind) {
                                Some(UpstreamKind::TransportCut) => {
                                    // 传输层掐断（issue #158 P2-2，与 Trae 池 sse.rs 同
                                    // 口径）：账号无关的传输故障——不冷却账号/模型、
                                    // 不解绑粘性、不计空完成指纹。未流出 → 换号（收尾
                                    // 帧未发）；已流出 → 就地收尾落账失败（终止帧已由
                                    // stream_forward_ex EOF-with-content 分支补发，
                                    // 客户端看到完整流，服务端如实记失败）
                                    if sent_any {
                                        state.record_usage_cached(super::usage::UsageBucket::Qoder, cached);
                                        state.record_usage_qoder(
                                            model, &win_uid, key_id, false, true, duration_ms,
                                            pt, ct, Some(ttfb_ms), credits, original_credits,
                                            &session_ctx_from_ids(&peek, win_session_ids.as_ref()),
                                        );
                                        state.logger.log_request_ttfb(
                                            "qoder", "POST", "/v1/chat/completions", model, true,
                                            200, &win_uid, duration_ms, Some(ttfb_ms), &key_name,
                                            &state.qoder_pool.name_of(&win_uid), Some(&msg),
                                        );
                                        return;
                                    }
                                    // PR #162：cut_hits 计入轮换耗尽归因——全池被掐时
                                    // exhaust_message_with_transport_cut 报传输掐断而非
                                    // 误报 "no healthy account available"（与 trae 池同口径）
                                    cut_hits += 1;
                                    break;
                                }
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
                                        // 中（并发请求刚记录过）则直接终止轮换返回
                                        // 429（移植 7883d1c），不再对池内每个账号硬打
                                        // 已知故障上游（原 break 换号会轮换到耗尽，
                                        // 逐账号真实请求）
                                        if model_cooling_remaining_secs(model).is_some()
                                            && !self_noted_server_cooldown
                                        {
                                            // 复用外层 duration_ms（与同分支其余记账同一口径）
                                            state.record_usage_qoder(
                                                model, &win_uid, key_id, false, true,
                                                duration_ms, 0, 0, None, None, None,
                                                &usage_ctx,
                                            );
                                            state.logger.log_request(
                                                "qoder", "POST", "/v1/chat/completions",
                                                model, true, 429, &win_uid, duration_ms,
                                                &key_name,
                                                &state.qoder_pool.name_of(&win_uid),
                                                Some("模型冷却中，终止轮换"),
                                            );
                                            send_stream_error(
                                                tx, proto, 429,
                                                &format!("model {model} cooling down, retry later"),
                                            );
                                            return;
                                        }
                                        // M10 审查收口：内层排队退避同样受轮换预算约束
                                        // ——3×30s 排队退避可独自耗尽客户端读超时窗口，
                                        // 外层预算检查永远没机会触发；超预算 break 换号
                                        // （外层预算收尾按 exhaust 语义错误必达）
                                        if Instant::now() + Duration::from_secs(secs as u64)
                                            > rotate_deadline
                                        {
                                            break;
                                        }
                                        note_model_cooldown(model, secs);
                                        queue_same += 1;
                                        if !sleep_interruptible(Duration::from_secs(secs), Some(tx)) {
                                            // 审查 M5：断连为请求终局，补记失败终态用量
                                            state.record_usage_qoder(
                                                model, &win_uid, key_id, false, true,
                                                start_ts.elapsed().as_millis() as u64, 0, 0,
                                                None, None, None, &session_ctx(&peek),
                                            );
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
                                        // issue #67（移植 main@36d628f）：硬冷却/禁用类错误解绑该账号粘性
                                        if ek.unbinds_sticky() {
                                            state.qoder_sticky.unbind_uid(&win_uid);
                                        }
                                    }
                                    // issue #90 修复①：上游服务端异常（5xx/业务 504）纳入
                                    // 模型级渐进冷却——仅在内容未流出时记账（sent_any=true
                                    // 说明模型实际在服务，尾部错误帧不代表模型故障）
                                    if kind == UpstreamKind::Server && !sent_any {
                                        note_model_server_cooldown(model);
                                        self_noted_server_cooldown = true; // M2：本请求自记
                                    }
                                    // issue #90 修复②：last_error 带模型名 + 时刻（此前只有
                                    // uid/code/msg，无法定位是哪个模型、故障从何时开始）
                                    *safe_lock(&state.last_error) = Some(format!(
                                        "qoder uid={} model={} code={} msg={} at {}",
                                        win_uid,
                                        model,
                                        code,
                                        safe_slice(&msg, 200),
                                        now_cn_timestamp()
                                    ));
                                }
                                None => {
                                    // translate 之外的错误帧。EOF 零完成哨兵（-9901）不经
                                    // translate 写 ErrMeta，必落本分支（非「理论不可达」）：
                                    // 上游间歇性空回放属暂态，按 wb_route 口径免熔断，
                                    // 仅由下方 !sent_any 分支换号重试
                                    if super::is_empty_completion(code, &msg) {
                                        empty_hits += 1; // 空完成计数：耗尽收尾区分指纹拦截
                                    } else {
                                        state.qoder_pool.note_error(&win_uid, ErrKind::Server);
                                    }
                                }
                            }
                            if !sent_any {
                                // 流未开始：错误不下发，允许换号重试（中间态不记账）
                                break;
                            }
                            // 就地收尾终局：记 usage（部分成功按实际 usage 落账）
                            state.record_usage_cached(super::usage::UsageBucket::Qoder, cached);
                            record_usage(false);
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
                                record_usage(false);
                                state.logger.log_request_ttfb(
                                    "qoder", "POST", "/v1/chat/completions", model, true, 200,
                                    &win_uid, duration_ms, Some(ttfb_ms), &key_name,
                                    &state.qoder_pool.name_of(&win_uid),
                                    Some("流内失败已透传客户端"),
                                );
                                return;
                            }
                            // P2-2：传输层掐断且内容已流出（翻译层 EOF 不补 [DONE] →
                            // stream_forward_ex 视作 EOF 断流并补终止帧，error_info 为
                            // 空，若无此检查会被当成功落账+绑粘性）——按失败落账，
                            // 不绑粘性、不记成功、不冷却（账号无关）
                            if matches!(
                                meta.as_ref().map(|m| m.kind),
                                Some(UpstreamKind::TransportCut)
                            ) {
                                state.record_usage_cached(super::usage::UsageBucket::Qoder, cached);
                                state.record_usage_qoder(
                                    model, &win_uid, key_id, false, true, duration_ms,
                                    pt, ct, Some(ttfb_ms), credits, original_credits,
                                    &session_ctx_from_ids(&peek, win_session_ids.as_ref()),
                                );
                                state.logger.log_request_ttfb(
                                    "qoder", "POST", "/v1/chat/completions", model, true, 200,
                                    &win_uid, duration_ms, Some(ttfb_ms), &key_name,
                                    &state.qoder_pool.name_of(&win_uid),
                                    Some("传输层掐断（内容已流出）→ 就地收尾落账失败"),
                                );
                                return;
                            }
                            // 成功终局：缓存读 tokens 旁路同落（M4；0 值不入账）
                            state.record_usage_cached(super::usage::UsageBucket::Qoder, cached);
                            record_usage(true);
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
                            // M10 审查收口：同号退避（最长 60s×N）受轮换预算约束，
                            // 超预算 break 换号 → 外层预算收尾按 exhaust 语义必达
                            if Instant::now() + Duration::from_millis(delay_ms.min(60_000) as u64)
                                > rotate_deadline
                            {
                                break;
                            }
                            // 审查修复：分段 sleep + 断连感知（原整段 sleep 最长 60s，
                            // 断连后仍空等；原 sleep 后的 is_closed 检查一并合并）
                            if !sleep_interruptible(
                                Duration::from_millis(delay_ms.min(60_000)),
                                Some(tx),
                            ) {
                                // 审查 M5：断连为请求终局，补记失败终态用量
                                state.record_usage_qoder(
                                    model, &picked.uid, key_id, false, true,
                                    start_ts.elapsed().as_millis() as u64, 0, 0,
                                    None, None, None, &session_ctx(&peek),
                                );
                                return;
                            }
                            continue;
                        }
                        RetryAction::SwitchKey => {
                            // Qoder 业务错误常在 403/200 信封里：按 upstream 分类；
                            // 排队（10605）同号退避优先——模型已在冷却中则直接
                            // 429 终止轮换（见下方冷却检查，移植 7883d1c）
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
                                // 审查修复：模型级冷却（同流内 Queued 分支）——
                                // 冷却中直接终止轮换返回 429（移植 7883d1c），不再逐账号硬打
                                if model_cooling_remaining_secs(model).is_some()
                                    && !self_noted_server_cooldown
                                {
                                    let duration_ms = start_ts.elapsed().as_millis() as u64;
                                    state.record_usage_qoder(
                                        model, &picked.uid, key_id, false, true,
                                        duration_ms, 0, 0, None, None, None,
                                        &session_ctx(&peek),
                                    );
                                    state.logger.log_request(
                                        "qoder", "POST", "/v1/chat/completions", model, true,
                                        429, &picked.uid, duration_ms, &key_name,
                                        &state.qoder_pool.name_of(&picked.uid),
                                        Some("模型冷却中，终止轮换"),
                                    );
                                    send_stream_error(
                                        tx, proto, 429,
                                        &format!("model {model} cooling down, retry later"),
                                    );
                                    return;
                                }
                                // M10：同号排队退避受轮换预算约束（同流内 Queued 分支）
                                if Instant::now() + Duration::from_secs(secs as u64)
                                    > rotate_deadline
                                {
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
                                // issue #67（移植 main@36d628f）：硬冷却/禁用类错误解绑该账号粘性
                                if kind.unbinds_sticky() {
                                    state.qoder_sticky.unbind_uid(&picked.uid);
                                }
                            }
                            // issue #90 修复①：HTTP 层 5xx（含 504）换号前记模型级
                            // 渐进冷却——否则 504 只逐账号轮换硬打，dispatch 预检
                            // 永不触发跨池回退（issue #90 主诉场景）
                            if classified.kind == UpstreamKind::Server {
                                note_model_server_cooldown(model);
                                self_noted_server_cooldown = true; // M2：本请求自记
                            }
                            *safe_lock(&state.last_error) = Some(format!(
                                "qoder uid={} model={} status={} body={} at {}",
                                picked.uid,
                                model,
                                status,
                                safe_slice(&resp_body, 200),
                                now_cn_timestamp()
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
                            let msg = format!(
                                "upstream {} error: {}{}",
                                status,
                                safe_slice(&resp_body, 300),
                                qoder_upstream::context_override_hint(&entry, status),
                            );
                            // 终态记账对齐「每请求一行」口径（移植 7883d1c，审查 F3）：
                            // Fatal 是请求终局（同 no-healthy 由取号分支记录），此前仅
                            // log_request 导致用量统计页缺口；ttfb/credits 无失败语义传 None
                            state.record_usage_qoder(model, &picked.uid, key_id, false, true,
                                start_ts.elapsed().as_millis() as u64, 0, 0,
                                None, None, None, &session_ctx(&peek));
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
        // issue #154：聚合路径同流式门控——Qoder 上游对图片输入 400 且报错信息
        // 不可读，入口显式拒绝给出可行动提示（与流式路径同文案）
        if body_has_image(&peek) {
            return Err(
                "upstream 400 error: Qoder 池暂不支持图片输入（图片上传通道未接线，issue #154），请移除图片或改用支持视觉的非 Qoder 模型"
                    .to_string(),
            );
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
        // issue #111：免费档模型（倍率 0）零消耗，取号豁免积分维度过滤，同流式路径。
        // 免费活动覆盖（资源调度页开关）在地区专属判定之后应用（同流式注释）
        let entry = {
            let mut e = entry;
            qoder_upstream::apply_flash_free_override(
                &mut e,
                state
                    .qoder_flash_free_rate
                    .load(std::sync::atomic::Ordering::Relaxed),
            );
            // Qoder 页单模型上下文长度 → parameters.context_length（未设置不下发）
            qoder_upstream::apply_context_override(
                &mut e,
                super::context_window::qoder_target(&state.data_dir, &model),
            );
            e
        };
        let free_model = qoder_upstream::entry_is_free(&entry);

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
                    state.qoder_pool.pick_by_uid(&b.uid, free_model).map(|_| b.uid)
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
                .pick_sticky_yield(u, allowed_set.as_ref(), free_model)
                .map(|(p, ev)| {
                    if let Some(ev) = ev {
                        state.logger.log_sched_event(&ev);
                    }
                    p
                })
        });

        // ── issue #158 P1-1：请求侧正向改写（同 run_qoder_stream 流式路径）──
        let sanitize = state.qoder_sanitize.load(std::sync::atomic::Ordering::Relaxed);
        let templates =
            super::wb_route::load_templates(&super::wb_route::WbView::wb(state.clone()));
        let tool_alias_rules =
            super::wb_route::load_tool_alias_rules(&super::wb_route::WbView::wb(state.clone()));
        let mut last_residual_note: Option<String> = None;
        // 会话稳定种子（同流式）：从粘性键派生，同会话跨请求恒定
        let seed_base = format!("q#{:?}", sticky_key);

        let mut tried: HashSet<String> = HashSet::new();
        let mut empty_hits: usize = 0; // 空完成计数：轮换耗尽收尾区分指纹拦截与账号耗尽
        // PR #162：传输掐断计数（聚合侧对齐流式路径）
        let mut cut_hits: usize = 0;
        // 轮换预算（issue #156 聚合侧补齐，对齐流式路径）：聚合含「首字超时 10s × N
        // 账号」逐号等待，无预算可拖过客户端读超时；仅约束「换号再试」（首个账号
        // tried 空不受影响），超预算按 exhaust 语义收尾
        let rotate_deadline = Instant::now() + Duration::from_secs(45);
        // M2 审查修复（同流式路径）：本请求自记的模型级 Server 冷却不触发
        // 各「冷却中终止」早退——否则首账号瞬时故障即终结整轮轮换
        let mut self_noted_server_cooldown = false;

        loop {
            // 轮换预算检查：超预算不再取新号，按 exhaust 语义收尾（与下方取号
            // 耗尽收尾逐行同构 + log_request(503)，非流式以 return Err 透传）
            if !tried.is_empty() && Instant::now() > rotate_deadline {
                let duration_ms = start_ts.elapsed().as_millis() as u64;
                state.record_usage_qoder(
                    &model, "none", &key_id, false, stream, duration_ms, 0, 0, None, None, None,
                    &session_ctx(&peek),
                );
                let (healthy_total, _) = state.qoder_pool.selectable_stats_in(None, free_model);
                let (hard_credit_cnt, _) = state.qoder_pool.hard_credit_stats();
                let (exhaust_msg, exhaust_tag) = super::routes::exhaust_message_with_transport_cut(
                    "qoder",
                    empty_hits,
                    tried.len(),
                    healthy_total,
                    hard_credit_cnt,
                    cut_hits,
                );
                // 模型级冷却与指纹判定同口径（对齐流式预算收尾）：空完成主导（指纹
                // 拦截）时模型本身可用，不记 Server 冷却——冷却误伤且跨池回退拦不住
                // 同指纹；仅非指纹主导（真 5xx/超时占多数）补记，后续请求预检快速失败
                if exhaust_tag != "CHANNEL_FINGERPRINT_BLOCK" {
                    note_model_server_cooldown(&model);
                }
                // issue #158 P1：池级指纹拦截短记忆（对齐流式/trae/buddy 池）——
                // 空完成主导（指纹拦截）才标记，TTL 600s 内同池新请求激进清洗起步
                super::wb_payload::mark_channel_fp_block("qoder", exhaust_tag == "CHANNEL_FINGERPRINT_BLOCK");
                let sample_note = super::wb_payload::empty_sample_note(
                    &exhaust_tag, "qoder", &body_vec,
                );
                if !sample_note.is_empty() {
                    state.logger.log_debug_line(format!(
                        "[DEBUG] {exhaust_tag} tried={} empty_hits={} cut_hits={} pool=qoder rotate_budget_exhausted{sample_note}",
                        tried.len(),
                        empty_hits,
                        cut_hits,
                    ));
                }
                state.logger.log_request(
                    "qoder", "POST", "/v1/chat/completions", &model, stream, 503, "none",
                    duration_ms, &key_name, "", Some(&exhaust_msg),
                );
                return Err(exhaust_msg);
            }
            // ── 取号：粘性命中优先，否则按 Key 约束 + 调度策略；换号后仅走策略 ──
            let picked = match first_pick.take() {
                Some(p) => p,
                None => match state.qoder_pool.pick_excluding_constrained_ev(
                    &tried,
                    allowed_set.as_ref(),
                    dedicated.as_deref(),
                    free_model,
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
                            None,
                            None,
                            &session_ctx(&peek),
                        );
                        // 审查 P2：轮换耗尽收尾对齐 routes.rs——空完成主导 → 指纹拦截语义
                        let (healthy_total, _) = state.qoder_pool.selectable_stats_in(None, free_model);
                        let (hard_credit_cnt, _) = state.qoder_pool.hard_credit_stats();
                        let (exhaust_msg, exhaust_tag) = super::routes::exhaust_message_with_transport_cut(
                            "qoder",
                            empty_hits,
                            tried.len(),
                            healthy_total,
                            hard_credit_cnt,
                            cut_hits,
                        );
                        // issue #158 P1：池级指纹拦截短记忆（对齐 trae/buddy 池）——空完成
                        // 主导（指纹拦截）才标记，TTL 600s 内同池新请求激进清洗起步
                        super::wb_payload::mark_channel_fp_block("qoder", exhaust_tag == "CHANNEL_FINGERPRINT_BLOCK");
                        // issue #99 ① + #125 增强：影子风控收尾采样入内存环形缓冲
                        // （kind=empty，非流式路径无收尾 [DEBUG] 行，就地补齐；三池同口径）
                        let sample_note = super::wb_payload::empty_sample_note(
                            &exhaust_tag, "qoder", &body_vec,
                        );
                        if !sample_note.is_empty() {
                            state.logger.log_debug_line(format!(
                                "[DEBUG] {exhaust_tag} tried={} empty_hits={} cut_hits={} pool=qoder{sample_note}",
                                tried.len(),
                                empty_hits,
                                cut_hits,
                            ));
                        }
                        return Err(exhaust_msg);
                    }
                },
            };
            tried.insert(picked.uid.clone());
            // 当前账号排队重试计数（账号局部：换号自然重置）
            let mut queue_same: u32 = 0;
            // 审查 P1-4 TOCTOU 收紧：普通候选 CAS 复核「在途 < 并发上限」，已满则
            // 不发起请求换下一候选（不计错误不冷却）；over_limit 候选（粘性锁定、
            // busy_fallback 降级）保留原无条件绑定语义。
            // active_uid 仅在绑定成功后更新（CAS 拒绝的候选不得污染「当前账号」展示）
            let bound = if picked.over_limit {
                guard = guard.bind_account(state.qoder_pool.inflight_handle(&picked.uid));
                true
            } else {
                guard.bind_account_cas(
                    state.qoder_pool.inflight_handle(&picked.uid),
                    state.qoder_pool.concurrency_limit(),
                )
            };
            if !bound {
                continue;
            }
            *safe_lock(&state.active_uid) = Some(picked.uid.clone());

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
                        // issue #67（移植 main@36d628f）：硬冷却/禁用类错误解绑该账号粘性
                        if kind.unbinds_sticky() {
                            state.qoder_sticky.unbind_uid(&picked.uid);
                        }
                        // 同流式分支：凭证解析错误原文脱敏截断（last_error 对外展示）
                        *safe_lock(&state.last_error) = Some(format!(
                            "qoder identity uid={} err={}",
                            picked.uid,
                            safe_slice(&e, 200)
                        ));
                        continue;
                    }
                },
            };

            // ── issue #158 P1-1：active_peek（清洗副本，同流式路径）──
            // 原始 peek 保留给 session_ctx 记账 preview；清洗+别名改写打在副本上
            let aggressive =
                sanitize && (empty_hits > 0 || super::wb_payload::fp_block_recent("qoder"));
            let mut active_peek: Value = peek.clone();
            // 逆映射供响应侧回改（P1-2：aggregate 流式收集点消费；resp 聚合后
            // 已是原名，闭包外 completion_to_anthropic 传 None 即可）
            let tool_alias: Option<std::collections::HashMap<String, String>>;
            if sanitize {
                let seed = super::wb_payload::variant_seed(
                    &seed_base,
                    (tried.len() - 1) as u64,
                );
                active_peek = super::payload::sanitize_openai_body(
                    &peek, &templates, seed, aggressive,
                );
                tool_alias = super::wb_payload::apply_tool_alias_forward_value(
                    &mut active_peek,
                    &tool_alias_rules,
                );
                // 残指纹自检（issue #122 C3）：debug 日志按请求去重
                if let Some(note) =
                    super::wb_payload::residual_fingerprint_note_value(&active_peek)
                {
                    if last_residual_note.as_deref() != Some(note.as_str()) {
                        last_residual_note = Some(note.clone());
                        state.logger.log_debug_line(format!(
                            "[DEBUG] 残指纹自检命中: {note} pool=qoder uid={}",
                            picked.uid
                        ));
                    }
                }
            } else {
                tool_alias = None;
            }

            let converted =
                match qoder_upstream::prepare_qoder_body(&active_peek, &entry, &creds.uid) {
                    Ok(b) => b,
                    Err(e) => return Err(e),
                };
            // 审查修复 K：信封 id 在外层解析一次，内层记账复用（同流式路径）
            let envelope_ids = extract_envelope_ids(&converted);

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
                            &active_peek,
                            &entry,
                            &model_key,
                            &model_source,
                            &tried,
                            allowed_set.as_ref(),
                            dedicated.as_deref(),
                            &envelope_ids,
                            converted.len(),
                        ) {
                            Ok(w) => w,
                            Err(()) => {
                                state.qoder_pool.note_error(&picked.uid, ErrKind::Server);
                                // issue #156 聚合侧补齐（对齐流式路径）：竞速双败
                                // 补记模型级 Server 冷却——只 note 账号时，后续请求
                                // 仍逐账号重演 10s 首字等待；补模型级冷却后走
                                // dispatch 预检快速失败/跨池回退
                                note_model_server_cooldown(&model);
                                self_noted_server_cooldown = true; // M2：本请求自记
                                break;
                            }
                        };
                        let ttfb_ms = ttfb_start.elapsed().as_millis() as u64;
                        // 对冲计数落定 + guard 重绑（接管时生效账号 = 对冲账号）
                        guard = settle_qoder_hedge(&state, &mut race, guard, &picked.uid);
                        hedge_taken = race.takeover;
                        let win_uid = race.uid.clone();
                        // G4：记账会话 id 按胜者信封落定（race 在聚合处被部分消费，先取快照）
                        let win_session_ids = race.session_ids.clone().or_else(|| envelope_ids.clone());
                        // 竞速胜者原始行源 → 翻译 → 聚合（tool_calls 合并/usage 收集）；
                        // 首字超时已在 race 内裁定，聚合阶段无失败通道。
                        // P2-2：胜者读错误槽随行源移交翻译器（EOF 掐断识别）
                        let agg_chat_id = format!("chatcmpl-{}", now_ts());
                        let translated = qoder_upstream::QoderTranslate::with_read_slot(
                            race.lines,
                            err_slot.clone(),
                            Some(race.read_slot.clone()),
                            &agg_chat_id,
                            &model,
                        );
                        // P1-2：流式收集阶段按胜者尝试的别名表回改工具名（resp 已是原名）
                        let (resp, error) =
                            wb_sse::aggregate(translated, &agg_chat_id, tool_alias.as_ref());
                        let duration_ms = start_ts.elapsed().as_millis() as u64;
                        // P2-2：传输层掐断优先于一切结果——聚合遇 EOF 掐断会返回
                        // (Some(部分响应), None)，直接当成功交付会把截断数据（可能
                        // 含残缺 tool_calls JSON）发给客户端；丢弃换号（非流式可安全重试）
                        if matches!(
                            err_slot.lock().unwrap_or_else(|e| e.into_inner()).as_ref().map(|m| m.kind),
                            Some(qoder_upstream::UpstreamKind::TransportCut)
                        ) {
                            state.logger.log_request_ttfb(
                                "qoder", "POST", "/v1/chat/completions", &model, stream, 200,
                                &win_uid, duration_ms, Some(ttfb_ms), &key_name,
                                &state.qoder_pool.name_of(&win_uid),
                                Some("传输层掐断（部分响应已丢弃）→ 换号重试"),
                            );
                            // PR #162：cut_hits 计入轮换耗尽归因（与流式路径同口径）
                            cut_hits += 1;
                            break;
                        }
                        match (resp, error) {
                            (Some(mut r), None) => {
                                r["model"] = json!(model);
                                // M1 修复（审查）：as_u64 窄口径改 wb_sse::u64_pair
                                // 宽口径（字符串/浮点形态不再静默得 0）
                                let (pt, ct) = r.get("usage").map(wb_sse::u64_pair).unwrap_or((0, 0));
                                let (credits, original_credits) =
                                    extract_billing(r.get("usage"));
                                // issue #111：免费档调度消耗按 0 落账（同流式路径注释）
                                let (credits, original_credits) = if free_model {
                                    (Some(0.0), Some(0.0))
                                } else {
                                    (credits, original_credits)
                                };
                                // M4：缓存读 tokens 旁路记账（成功终局，0 值不入账）
                                state.record_usage_cached(
                                    super::usage::UsageBucket::Qoder,
                                    r.get("usage").map(wb_sse::cached_tokens_of).unwrap_or(0),
                                );
                                state.record_usage_qoder(
                                    &model, &win_uid, &key_id, true, stream, duration_ms, pt, ct,
                                    Some(ttfb_ms),
                                    credits,
                                    original_credits,
                                    // 审查修复 G4：session_ctx 用胜者信封 ids（接管时=对冲侧）
                                    &session_ctx_from_ids(&peek, win_session_ids.as_ref()),
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
                                // 信封 400 拒绝 context_length 覆盖：终局返回原错误 +
                                // 清空指引（同流式分支）
                                if let Some(full) = envelope_context_reject(&entry, meta.as_ref()) {
                                    state.record_usage_qoder(
                                        &model, &win_uid, &key_id, false, stream, duration_ms,
                                        0, 0, Some(ttfb_ms), None, None,
                                        &session_ctx_from_ids(&peek, win_session_ids.as_ref()),
                                    );
                                    state.logger.log_request_ttfb(
                                        "qoder", "POST", "/v1/chat/completions", &model, stream,
                                        400, &win_uid, duration_ms, Some(ttfb_ms), &key_name,
                                        &state.qoder_pool.name_of(&win_uid), Some(&full),
                                    );
                                    return Err(full);
                                }
                                match meta.as_ref().map(|m| m.kind) {
                                    Some(UpstreamKind::TransportCut) => {
                                        // 传输层掐断（issue #158 P2-2）：账号无关的传输
                                        // 故障——不冷却账号/模型、不解绑粘性、不计空完成
                                        // 指纹；下方公共收尾统一落 last_error + 日志 +
                                        // break 换号重试。PR #162：计入 cut_hits，全池
                                        // 被掐时耗尽收尾改报传输掐断而非误报无健康账号
                                        cut_hits += 1;
                                    }
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
                                            // 审查修复：模型级冷却（同流式分支）——
                                            // 冷却中直接终止轮换返回 429（移植 7883d1c，
                                            // upstream 前缀借助端点状态码透传映射），
                                            // 不再逐账号硬打已知故障上游
                                            if model_cooling_remaining_secs(&model).is_some()
                                                && !self_noted_server_cooldown
                                            {
                                                state.record_usage_qoder(
                                                    &model, &win_uid, &key_id, false, stream,
                                                    duration_ms, 0, 0, None, None, None,
                                                    &session_ctx(&peek),
                                                );
                                                state.logger.log_request_ttfb(
                                                    "qoder", "POST", "/v1/chat/completions",
                                                    &model, stream, 429, &win_uid, duration_ms,
                                                    Some(ttfb_ms), &key_name,
                                                    &state.qoder_pool.name_of(&win_uid),
                                                    Some("模型冷却中，终止轮换"),
                                                );
                                                return Err(format!(
                                                    "upstream 429 error: model {model} cooling down, retry later"
                                                ));
                                            }
                                            // M10：内层排队退避受轮换预算约束（同流式路径）
                                            if Instant::now() + Duration::from_secs(secs as u64)
                                                > rotate_deadline
                                            {
                                                break;
                                            }
                                            note_model_cooldown(&model, secs);
                                            queue_same += 1;
                                            // 统一走 sleep_interruptible(None)（移植 7883d1c）：
                                            // 分段等待封装统一，语义等同整段 sleep
                                            sleep_interruptible(Duration::from_secs(secs), None);
                                            continue;
                                        }
                                        break; // 超限换号
                                    }
                                    Some(kind) => {
                                        let ek = kind.to_err_kind();
                                        if ek != ErrKind::None {
                                            state.qoder_pool.note_error(&win_uid, ek);
                                            // issue #67 审查修复（移植 main@36d628f）：账号级长冷却解绑粘性（与流式分支口径对齐）
                                            if ek.unbinds_sticky() {
                                                state.qoder_sticky.unbind_uid(&win_uid);
                                            }
                                        }
                                        // issue #90 修复①：同流式分支（非流式无部分流出概念）
                                        if kind == UpstreamKind::Server {
                                            note_model_server_cooldown(&model);
                                            self_noted_server_cooldown = true; // M2：本请求自记
                                        }
                                    }
                                    None => {
                                        state.qoder_pool.note_error(&win_uid, ErrKind::Server);
                                    }
                                }
                                *safe_lock(&state.last_error) = Some(format!(
                                    "qoder uid={} model={} code={} msg={} at {}",
                                    win_uid,
                                    model,
                                    code,
                                    safe_slice(&msg, 200),
                                    now_cn_timestamp()
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
                                // (None, None)：EOF 零完成（影子风控/上游异常）——计数后换号
                                empty_hits += 1;
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
                                // M10 审查收口：同号退避（最长 60s×N）受轮换预算约束，
                                // 超预算 break 换号 → 外层预算收尾按 exhaust 语义必达
                                if Instant::now()
                                    + Duration::from_millis(delay_ms.min(60_000) as u64)
                                    > rotate_deadline
                                {
                                    break;
                                }
                                // 统一走 sleep_interruptible(None)：分段等待封装统一
                                sleep_interruptible(
                                    Duration::from_millis(delay_ms.min(60_000)),
                                    None,
                                );
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
                                    // 审查修复：模型级冷却（同流式分支）——冷却中
                                    // 直接终止轮换返回 429（移植 7883d1c），不再逐账号硬打；
                                    // M2：本请求自记的冷却不触发本早退
                                    if model_cooling_remaining_secs(&model).is_some()
                                        && !self_noted_server_cooldown
                                    {
                                        state.record_usage_qoder(&model, &picked.uid, &key_id,
                                            false, stream,
                                            start_ts.elapsed().as_millis() as u64, 0, 0,
                                            None, None, None, &session_ctx(&peek));
                                        state.logger.log_request(
                                            "qoder", "POST", "/v1/chat/completions", &model,
                                            stream, 429, &picked.uid,
                                            start_ts.elapsed().as_millis() as u64, &key_name,
                                            &state.qoder_pool.name_of(&picked.uid),
                                            Some("模型冷却中，终止轮换"),
                                        );
                                        return Err(format!(
                                            "upstream 429 error: model {model} cooling down, retry later"
                                        ));
                                    }
                                    // M10：内层排队退避受轮换预算约束（同流式路径）
                                    if Instant::now() + Duration::from_secs(secs as u64)
                                        > rotate_deadline
                                    {
                                        break;
                                    }
                                    note_model_cooldown(&model, secs);
                                    queue_same += 1;
                                    // 统一走 sleep_interruptible(None)：分段等待封装统一
                                    sleep_interruptible(Duration::from_secs(secs), None);
                                    continue;
                                }
                                let kind = classified.kind.to_err_kind();
                                if kind != ErrKind::None {
                                    state.qoder_pool.note_error(&picked.uid, kind);
                                    // issue #67（移植 main@36d628f）：硬冷却/禁用类错误解绑该账号粘性
                                    if kind.unbinds_sticky() {
                                        state.qoder_sticky.unbind_uid(&picked.uid);
                                    }
                                }
                                // issue #90 修复①：同流式 HTTP 分支（5xx 换号前记模型级冷却）
                                if classified.kind == UpstreamKind::Server {
                                    note_model_server_cooldown(&model);
                                    self_noted_server_cooldown = true; // M2：本请求自记
                                }
                                *safe_lock(&state.last_error) = Some(format!(
                                    "qoder uid={} model={} status={} body={} at {}",
                                    picked.uid,
                                    model,
                                    status,
                                    safe_slice(&resp_body, 200),
                                    now_cn_timestamp()
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
                                // 终态记账对齐「每请求一行」口径（移植 7883d1c，审查 F3）：
                                // Fatal 是请求终局（同 no-healthy 由取号分支记录），此前仅
                                // log_request 导致用量统计页缺口
                                state.record_usage_qoder(&model, &picked.uid, &key_id, false, stream,
                                    start_ts.elapsed().as_millis() as u64, 0, 0,
                                    None, None, None, &session_ctx(&peek));
                                state.logger.log_request(
                                    "qoder", "POST", "/v1/chat/completions", &model, stream,
                                    status, &picked.uid,
                                    start_ts.elapsed().as_millis() as u64, &key_name,
                                    &state.qoder_pool.name_of(&picked.uid),
                                    Some(&safe_slice(&resp_body, 300)),
                                );
                                return Err(format!(
                                    "upstream {} error: {}{}",
                                    status,
                                    safe_slice(&resp_body, 300),
                                    qoder_upstream::context_override_hint(&entry, status),
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
                // tool_alias 传 None：resp 来自闭包内 aggregate（流式收集阶段已回改
                // 原名，逆映射此处幂等无操作）；且 tool_alias 被 move 进
                // spawn_blocking 闭包，作用域不可达
                Protocol::Anthropic => wb_sse::completion_to_anthropic(
                    &resp,
                    &format!("msg_{}", now_ts()),
                    &model_out,
                    None,
                ),
                Protocol::OpenAiText => wb_sse::completion_to_text(&resp, &model_out),
                Protocol::Responses => super::wb_responses::completion_to_responses(
                    &resp,
                    &format!("resp_{}", now_ts()),
                    &model_out,
                    // Qoder 无 namespace 展平编码（Responses 分支为防御性兜底）
                    &std::collections::HashSet::new(),
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
            // 401/403 维持 503 语义；429 为模型冷却主动限流（移植 7883d1c），
            // 透传让客户端按 Retry-After 语义退避而非立即重试
            let upstream_status = msg
                .strip_prefix("upstream ")
                .and_then(|rest| rest.split(' ').next())
                .and_then(|s| s.parse::<u16>().ok());
            let status = match upstream_status {
                Some(s) if (400..500).contains(&s) && !matches!(s, 401 | 403) => {
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
    let msg = stream_msg_or_fallback(msg, code);
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

/// 字符串 code 变体（轮换耗尽收尾专用）：错误 code 保持字符串 "no_healthy_account"
/// 不变（客户端兼容，工程惯例）——i64 版会把它变数字。三协议分流形态同构
/// send_stream_error；耗尽文案 exhaust_message 恒非空，空 msg 兜底用 code
fn send_stream_error_named(
    tx: &tokio::sync::mpsc::Sender<Result<bytes::Bytes, std::io::Error>>,
    proto: Protocol,
    code: &str,
    msg: &str,
) {
    let msg = if msg.trim().is_empty() { code } else { msg };
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
                "error": {"code": code, "message": msg},
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

    // ==================== issue #90 修复①：Server 渐进模型级冷却 ====================

    /// Server 冷却按「失败片段」渐进（窗口过期才递增）且封顶；成功清除后归零。
    /// 冷却表是进程级共享静态，用独占模型名（前缀 + 计数器）避免并行测试互扰。
    #[test]
    fn server_cooldown_progressive_capped_and_cleared() {
        static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let model = format!(
            "t90-prog-{}",
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        assert!(model_cooling_remaining_secs(&model).is_none());
        // 片段 1：基准 30s
        note_model_server_cooldown(&model);
        let r1 = model_cooling_remaining_secs(&model).unwrap();
        assert!(r1 > 28 && r1 <= 30, "首次失败应为 30s 基准，实际 {r1}");
        // 同片段内连续记账（一次请求逐账号换号场景，窗口未过期）：
        // 不递增片段数，窗口仅延长（剩余仍 ≈30s，绝不翻倍）
        for _ in 0..4 {
            note_model_server_cooldown(&model);
        }
        let (_, fails) = *cooldown_map()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&model)
            .unwrap();
        assert_eq!(fails, 1, "窗口未过期时连续记账不得递增片段数");
        // 人为推进到窗口过期（不真等 30s）：片段 2 → 翻倍 60s
        cooldown_map()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(model.clone(), (Instant::now() - Duration::from_secs(1), 1));
        note_model_server_cooldown(&model);
        let r2 = model_cooling_remaining_secs(&model).unwrap();
        assert!(r2 > 55 && r2 <= 60, "过期后新片段应翻倍至 60s，实际 {r2}");
        // 连续失败片段至封顶：窗口恒 ≤ 600s
        for _ in 0..10 {
            let cur = cooldown_map()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&model)
                .copied()
                .unwrap();
            cooldown_map()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(model.clone(), (Instant::now() - Duration::from_secs(1), cur.1));
            note_model_server_cooldown(&model);
        }
        let capped = model_cooling_remaining_secs(&model).unwrap();
        assert!(capped <= SERVER_COOLDOWN_MAX_SECS, "冷却窗口应封顶 600s，实际 {capped}");
        // 成功清除 → 无冷却（片段计数随之归零）
        clear_model_cooldown(&model);
        assert!(model_cooling_remaining_secs(&model).is_none());
    }

    /// 排队退避（note_model_cooldown）不缩短既有 Server 冷却窗口：排队建议值
    /// （钳制 ≤30s）到达时若并发请求刚记了更长的 Server 窗口，应取较大者。
    #[test]
    fn queued_cooldown_does_not_shorten_server_window() {
        static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let model = format!(
            "t90-queue-{}",
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        // 两次 Server 失败 → 窗口 60s
        note_model_server_cooldown(&model);
        note_model_server_cooldown(&model);
        let before = model_cooling_remaining_secs(&model).unwrap();
        // 排队退避建议 5s：不得把 60s 窗口缩短
        note_model_cooldown(&model, 5);
        let after = model_cooling_remaining_secs(&model).unwrap();
        assert!(
            after >= before.saturating_sub(1),
            "排队建议值不得缩短既有 Server 窗口（{before} → {after}）"
        );
        clear_model_cooldown(&model);
    }

    /// 回归：排队退避先插入 fails=0 条目（note_model_cooldown 的 or_insert 路径），
    /// 随后 Server 失败进入 and_modify —— fails=0 不得下溢（曾为 `*fails - 1`，
    /// debug panic / release 冷却被错误推到 480s），应保持基准 30s 且计数不动
    #[test]
    fn server_cooldown_after_queued_only_entry_no_underflow() {
        static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let model = format!(
            "t90-queue0-{}",
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        // 排队退避先落条目：fails = 0、窗口 5s
        note_model_cooldown(&model, 5);
        // Server 失败（移植 7883d1c）：fails=0 的排队短窗不足以覆盖 5xx——
        // 窗口抬到基准 30s 但不递增计数（同片段语义）；也不得下溢
        // （曾为 `*fails - 1`：debug panic / release 冷却被错误推到上限）
        note_model_server_cooldown(&model);
        let r = model_cooling_remaining_secs(&model).unwrap();
        assert!(r > 28 && r <= 30, "fails=0 排队短窗应被 Server 失败抬到基准 30s，实际 {r}");
        let (_, fails) = *cooldown_map()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&model)
            .unwrap();
        assert_eq!(fails, 0, "短窗抬升不递增失败计数（同片段语义）");
        // 窗口过期后同一 Server 失败 → 开启新片段，按计数递增到基准 30s
        cooldown_map()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(model.clone(), (Instant::now() - Duration::from_secs(1), 0));
        note_model_server_cooldown(&model);
        let r2 = model_cooling_remaining_secs(&model).unwrap();
        assert!(r2 > 28 && r2 <= 30, "fails=0 → 过期后新片段取 30s 基准，实际 {r2}");
        clear_model_cooldown(&model);
    }

    /// issue #82 T2.3：credits 复数优先（新版字段），单数 credit 为旧版兜底
    #[test]
    fn extract_billing_prefers_plural_credits() {
        let u = json!({"credits": 12.5, "credit": 0, "original_credits": 25.0});
        let (c, o) = extract_billing(Some(&u));
        assert_eq!(c, Some(12.5));
        assert_eq!(o, Some(25.0));
        // 仅有旧版单数字段时兜底可读（新调用方 None 缺省不再走此路）
        let old = json!({"credit": 7});
        let (c, o) = extract_billing(Some(&old));
        assert_eq!(c, Some(7.0));
        assert_eq!(o, None);
    }

    /// issue #82 T2.3：双字段均缺 → None（调用方不入账，不得伪造 0）
    #[test]
    fn extract_billing_missing_fields_yields_none() {
        assert_eq!(extract_billing(None), (None, None));
        let empty = json!({"prompt_tokens": 10, "completion_tokens": 5});
        assert_eq!(extract_billing(Some(&empty)), (None, None));
        let nulls = json!({"credits": null});
        assert_eq!(extract_billing(Some(&nulls)), (None, None));
    }

    /// issue #82 T2.3：负值/非有限值视为不可信，整体不入账
    #[test]
    fn extract_billing_rejects_negative() {
        let neg = json!({"credits": -3.0});
        assert_eq!(extract_billing(Some(&neg)), (None, None));
        let neg_pair = json!({"credits": 5.0, "original_credits": -1.0});
        let (c, o) = extract_billing(Some(&neg_pair));
        assert_eq!(c, Some(5.0));
        assert_eq!(o, None);
    }

    /// 会话 preview 剥离 system-reminder 注入块与 <user_input> 包裹标签：
    /// agent 客户端把提醒拼在正文前、输入用标签包裹，preview 必须取剥离后的
    /// 用户原话（多块 / 跨行 / 前置残留空白）。
    #[test]
    fn session_preview_strips_system_reminder_blocks() {
        // 典型形态：多块 reminder 前置 + <user_input> 包裹的用户原话
        let peek = json!({"messages": [{"role": "user", "content":
            "<system-reminder>\nTodos have changed...\n# Todos\n- x\n</system-reminder>\n\n<system-reminder>The maximum number of terminals is 5</system-reminder>\n<user_input>\n说一个100字笑话\n</user_input>"}]});
        assert_eq!(session_preview(&peek), "说一个100字笑话");

        // 仅 <user_input> 包裹、无 reminder
        let wrapped = json!({"messages": [{"role": "user", "content":
            "<user_input>写一个100字笑话</user_input>"}]});
        assert_eq!(session_preview(&wrapped), "写一个100字笑话");

        // 剥离后仅剩空白 → 空串（调用方按无 preview 处理，不展示注入内容）
        let only_reminder = json!({"messages": [{"role": "user", "content":
            "<system-reminder>纯注入</system-reminder>"}]});
        assert_eq!(session_preview(&only_reminder), "");

        // 无 reminder 的普通消息不受影响；末条 user 才是取值来源
        let plain = json!({"messages": [
            {"role": "user", "content": "第一条"},
            {"role": "assistant", "content": "回复"},
            {"role": "user", "content": "<system-reminder>x</system-reminder> 写一个100字笑话"}
        ]});
        assert_eq!(session_preview(&plain), "写一个100字笑话");

        // reminder 之后再无正文 → 空；reminder 内含 "</system-reminder>" 字样也按整块匹配（非贪婪）
        let empty_tail = json!({"messages": [{"role": "user", "content":
            "前文<system-reminder>注入</system-reminder>  \n"}]});
        assert_eq!(session_preview(&empty_tail), "前文");
    }

    /// issue #154：三协议图片形态统一识别（image_url / image / input_image），
    /// 纯文本/字符串 content 恒无图；tool_result.content 内嵌图片递归命中
    #[test]
    fn body_has_image_detects_all_protocols() {
        // OpenAI 形态
        assert!(body_has_image(&json!({"messages": [{"role": "user", "content": [
            {"type": "text", "text": "看图"},
            {"type": "image_url", "image_url": {"url": "https://example.com/a.png"}}
        ]}]})));
        // Anthropic 形态
        assert!(body_has_image(&json!({"messages": [{"role": "user", "content": [
            {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "aGk="}}
        ]}]})));
        // Responses 形态
        assert!(body_has_image(&json!({"messages": [{"role": "user", "content": [
            {"type": "input_image", "image_url": "https://example.com/b.png"}
        ]}]})));
        // tool_result.content 内嵌图片
        assert!(body_has_image(&json!({"messages": [{"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "t1", "content": [
                {"type": "image", "source": {"type": "url", "url": "https://example.com/c.png"}}
            ]}
        ]}]})));
        // 纯文本：无图
        assert!(!body_has_image(&json!({"messages": [{"role": "user", "content": "hi"}]})));
        assert!(!body_has_image(&json!({"messages": [{"role": "user", "content": [
            {"type": "text", "text": "纯文字"}
        ]}]})));
        // 无 messages / 空 content
        assert!(!body_has_image(&json!({})));
        assert!(!body_has_image(&json!({"messages": [{"role": "user"}]})));
    }

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

    /// codex 审查【1】：HTTP 200 + 信封 statusCodeValue=400 走解码错误分支，
    /// 此前只换号轮换、最终报「无健康账号」，context_length 清空指引丢失。
    /// 完整复现解码链（信封行 → QoderTranslate → wb_sse::aggregate → ErrMeta）
    fn decode_envelope_err(envelope: Value) -> Option<ErrMeta> {
        let slot: Arc<Mutex<Option<ErrMeta>>> = Arc::new(Mutex::new(None));
        let lines = vec![format!("data: {envelope}"), String::new()].into_iter();
        let translated =
            qoder_upstream::QoderTranslate::new(lines, slot.clone(), "chatcmpl-t", "m");
        let (resp, error) = wb_sse::aggregate(translated, "chatcmpl-t", None);
        assert!(resp.is_none() && error.is_some(), "信封错误应落解码错误分支");
        let meta = slot.lock().unwrap().clone();
        meta
    }

    #[test]
    fn envelope_400_with_context_override_surfaces_hint() {
        let decode = decode_envelope_err;
        let raw = "{\"code\":\"InvalidParameter\",\"message\":\"context_length out of range\"}";
        let meta = decode(json!({"statusCodeValue": 400, "body": raw}));

        let mut set = json!({"id": "m", "upstreamKey": "m"});
        qoder_upstream::apply_context_override(&mut set, Some(1_000_000));
        let msg = envelope_context_reject(&set, meta.as_ref()).expect("应终局附指引");
        assert!(msg.starts_with("upstream 400 error: "), "{msg}");
        assert!(msg.contains("context_length out of range"), "保留上游原错误: {msg}");
        assert!(msg.contains("context_length=1000000"), "附清空指引: {msg}");

        // 未设置覆盖：维持原有轮换
        let unset = json!({"id": "m", "upstreamKey": "m"});
        assert!(envelope_context_reject(&unset, meta.as_ref()).is_none());
        // 非 400 / 有既定语义的业务码（排队）不劫持
        let m500 = decode(json!({"statusCodeValue": 500, "body": "boom"}));
        assert!(envelope_context_reject(&set, m500.as_ref()).is_none());
        let queued = decode(json!({"statusCodeValue": 400, "body": "{\"code\":\"10605\"}"}));
        assert!(envelope_context_reject(&set, queued.as_ref()).is_none());
    }

    /// codex 复核：上游参数错误正文 `context_length exceeded the model limit` 含
    /// "exceeded"，此前被额度泛词判成 Quota——上下文被拒分支（仅接 Unknown）
    /// 不命中，继续换号并给健康账号记 HardCredit。须先于额度泛词识别为参数
    /// 被拒，同时不吞 110 额度 / 10605 排队 / 401 / 403 账号错误
    #[test]
    fn envelope_context_length_exceeded_is_param_reject_not_quota() {
        let mut set = json!({"id": "m", "upstreamKey": "m"});
        qoder_upstream::apply_context_override(&mut set, Some(1_000_000));
        let raw = "{\"code\":\"InvalidParameter\",\"message\":\"参数错误: context_length exceeded the model limit\"}";
        let meta = decode_envelope_err(json!({"statusCodeValue": 400, "body": raw}));
        assert_eq!(meta.as_ref().map(|m| m.kind), Some(UpstreamKind::Unknown), "{meta:?}");
        let msg = envelope_context_reject(&set, meta.as_ref()).expect("应终局附指引，不换号");
        assert!(msg.contains("context_length exceeded the model limit"), "{msg}");
        assert!(msg.contains("context_length=1000000"), "{msg}");

        // 账号错误码即便正文同样点名 context_length 也维持原分类、不劫持
        let cases = [
            (400, "{\"code\":\"110\",\"message\":\"context_length exceeded\"}", UpstreamKind::Quota),
            (400, "{\"code\":\"10605\",\"message\":\"context_length exceeded\"}", UpstreamKind::Queued),
            (401, "context_length exceeded", UpstreamKind::Auth),
            (403, "context_length exceeded", UpstreamKind::Quota),
        ];
        for (status, body, want) in cases {
            let meta = decode_envelope_err(json!({"statusCodeValue": status, "body": body}));
            assert_eq!(meta.as_ref().map(|m| m.kind), Some(want), "{status} {body}");
            assert!(envelope_context_reject(&set, meta.as_ref()).is_none(), "{status} {body}");
        }
    }
}
